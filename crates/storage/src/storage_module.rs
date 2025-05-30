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
use base58::ToBase58;
use derive_more::derive::{Deref, DerefMut};
use eyre::{ensure, eyre, Context, OptionExt, Result};
use irys_database::{
    db::IrysDatabaseExt as _,
    submodule::{
        add_data_path_hash_to_offset_index, add_full_data_path, add_full_tx_path,
        add_start_offset_to_data_root_index, add_tx_path_hash_to_offset_index,
        clear_submodule_database, create_or_open_submodule_db, get_data_path_by_offset,
        get_data_size_by_data_root, get_start_offsets_by_data_root, get_tx_path_by_offset,
        set_data_size_for_data_root, tables::RelativeStartOffsets,
    },
};
use irys_packing::{capacity_single::compute_entropy_chunk, packing_xor_vec_u8};
use irys_types::{
    app_state::DatabaseProvider,
    get_leaf_proof, ledger_chunk_offset_ie,
    partition::{PartitionAssignment, PartitionHash},
    partition_chunk_offset_ii, Address, Base64, ChunkBytes, ChunkDataPath, ChunkPathHash, Config,
    DataLedger, DataRoot, LedgerChunkOffset, LedgerChunkRange, PackedChunk, PartitionChunkOffset,
    PartitionChunkRange, ProofDeserialize, RelativeChunkOffset, TxChunkOffset, TxPath,
    UnpackedChunk, H256,
};
use nodit::{interval::ii, InclusiveInterval, Interval, NoditMap, NoditSet};
use openssl::sha;
use reth_db::Database;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, RwLockReadGuard},
};
use tracing::{debug, error, info};

type SubmodulePath = PathBuf;

// In-memory chunk data indexed by offset within partition
type ChunkMap = BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>;

/// Storage submodules mapped to their chunk ranges
type SubmoduleMap =
    NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, StorageSubmodule>;

/// Tracks storage state of chunk ranges across all submodules
type StorageIntervals = NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, ChunkType>;

/// Maps a logical partition (fixed size) to physical storage across multiple drives
#[derive(Debug)]
pub struct StorageModule {
    /// an integer uniquely identifying the module
    pub id: usize,
    /// The (Optional) info about a partition assigned to this storage module
    pub partition_assignment: RwLock<Option<PartitionAssignment>>,
    /// In-memory chunk buffer awaiting disk write
    pending_writes: Arc<RwLock<ChunkMap>>,
    /// Tracks the storage state of each chunk across all submodules
    intervals: Arc<RwLock<StorageIntervals>>,
    /// Physical storage locations indexed by chunk ranges
    submodules: SubmoduleMap,
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
    /// pub submodules: Vec<(Interval<PartitionChunkOffset>, SubmodulePath)>,
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackingParams {
    pub packing_address: Address,
    pub partition_hash: Option<H256>,
    pub ledger: Option<u32>,
    pub slot: Option<usize>,
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
        fs::write(&path, toml).unwrap_or_else(|_| panic!("Failed to write config to {:?}", path));
    }
}

static PACKING_PARAMS_FILE_NAME: &str = "packing_params.toml";

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
    Ok(AtomicWriteFile::options()
        .read(true)
        .open(&path)
        .wrap_err_with(|| format!("Failed to create or open atomic file for {:?}", &path))?)
}

/// Defines how chunk data is processed and stored
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChunkType {
    /// Chunk containing matrix-packed entropy only
    Entropy,
    /// Chunk containing packed blockchain transaction data
    Data,
    /// Chunk has not been initialized
    Uninitialized,
}

// we can't put this in `types` due to dependency cycles
#[derive(Debug, Clone, Deref, DerefMut)]
pub struct StorageModules(pub StorageModuleVec);

pub type StorageModuleVec = Vec<Arc<StorageModule>>;

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct StorageModulesReadGuard {
    storage_module_data: Arc<RwLock<Vec<Arc<StorageModule>>>>,
}

impl StorageModulesReadGuard {
    /// Creates a new `ReadGuard` for StorageModules list
    pub const fn new(storage_module_data: Arc<RwLock<Vec<Arc<StorageModule>>>>) -> Self {
        Self {
            storage_module_data,
        }
    }

    /// Accessor method to get a read guard for the StorageModules list
    pub fn read(&self) -> RwLockReadGuard<'_, Vec<Arc<StorageModule>>> {
        self.storage_module_data.read().unwrap()
    }
}

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

            tracing::info!(?sub_base_path);
            fs::create_dir_all(&sub_base_path)?; // Ensure the directory exists (for component tests)

            // Get a file handle to the chunks.data file in the submodule
            let path = sub_base_path.join("chunks.dat");
            let chunks_file: Arc<Mutex<File>> = Arc::new(Mutex::new(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true) // Optional: creates file if it doesn't exist
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
            if params_path.exists() == false {
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
                let params = PackingParams::from_toml(params_path).expect("packing params to load");
                let pa = storage_module_info.partition_assignment.unwrap();
                ensure!(
                    params.packing_address == config.node_config.miner_address(),
                    "Active mining address: {} does not match partition packing address {}",
                    config.node_config.miner_address(),
                    params.packing_address
                );
                ensure!(params.partition_hash == Some(pa.partition_hash),
                    "Partition hash mismatch:\nexpected: {}\nfound   : {}\n\nError: Submodule partition assignments are out of sync with genesis block. \
                    This occurs when a new genesis block is created with a different last_epoch_hash, but submodules still have partition_hashes \
                    assigned from the previous genesis. To fix: clear the contents of the submodule directories and let them be repacked with the current genesis",
                    pa.partition_hash.0.to_base58(),
                    params.partition_hash.unwrap().0.to_base58(),
                );
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
                    submodule_interval.clone(),
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
            let _ = global_intervals.insert_merge_touching_if_values_equal(
                submodule_interval,
                ChunkType::Uninitialized,
            );
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
        if &gaps != &expected {
            return Err(eyre!(
                "Invalid storage module config, expected range {:?}, got range {:?}",
                &expected,
                &gaps
            ));
        }

        // Attempt to load a global set of intervals from the submodules
        let loaded_intervals = Self::load_intervals_from_submodules(&submodule_map);

        Ok(StorageModule {
            id: storage_module_info.id,
            partition_assignment: RwLock::new(storage_module_info.partition_assignment),
            pending_writes: Arc::new(RwLock::new(ChunkMap::new())),
            intervals: Arc::new(RwLock::new(loaded_intervals)),
            submodules: submodule_map,
            config: config.clone(),
        })
    }

    pub fn assign_partition(&self, partition_assignment: PartitionAssignment) {
        let mut pa = self.partition_assignment.write().unwrap();
        *pa = Some(partition_assignment);

        // Also update the packing params file in each submodule
        for (_, submodule) in self.submodules.iter() {
            let params = PackingParams {
                packing_address: self.config.node_config.miner_address(),
                partition_hash: Some(partition_assignment.partition_hash),
                ledger: partition_assignment.ledger_id,
                slot: partition_assignment.slot_index,
            };

            let params_path = submodule.path.join(PACKING_PARAMS_FILE_NAME);
            params.write_to_disk(&params_path);
        }
    }

    /// Returns the StorageModules partition_hash if assigned
    pub fn partition_hash(&self) -> Option<PartitionHash> {
        let pa = self.partition_assignment.read().unwrap();
        if let Some(part_assign) = *pa {
            Some(part_assign.partition_hash)
        } else {
            None
        }
    }

    pub fn partition_assignment(&self) -> Option<PartitionAssignment> {
        let pa = self.partition_assignment.read().unwrap();
        *pa
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
            submodule
                .db
                .update_eyre(|tx| clear_submodule_database(tx))?;
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
        let threshold = self.config.node_config.storage.num_writes_before_sync;
        let arc = self.pending_writes.clone();

        // First use read lock to check if we have work to do
        let write_batch = {
            let pending = arc.read().unwrap();

            let pending_writes = self
                .submodules
                .iter()
                .flat_map(|(interval, _)| {
                    let submodule_writes: Vec<_> = pending
                        .iter()
                        .filter(|(offset, _)| interval.contains_point(**offset))
                        .map(|(offset, state)| (*offset, state.clone()))
                        .collect();

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

        // Only acquire write lock if we have work to do
        if !write_batch.is_empty() {
            let mut pending = arc.write().unwrap();

            for (chunk_offset, (bytes, chunk_type)) in write_batch {
                // self.intervals are updated by write_chunk_internal()
                self.write_chunk_internal(chunk_offset, bytes, chunk_type.clone())?;
                pending.remove(&chunk_offset); // Clean up written chunks
            }
            drop(pending);

            // Save the updated intervals
            if self.write_intervals_to_submodules().is_err() {
                error!("Could not update submodule interval files");
            }
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
    fn load_intervals_from_submodules(submodules: &SubmoduleMap) -> StorageIntervals {
        let mut global_intervals = StorageIntervals::new();
        for (_, submodule) in submodules.iter() {
            let file = submodule
                .intervals_file
                .lock()
                .expect("to lock the submodule intervals file mutex");
            let submodule_intervals =
                read_intervals_file(&file).expect("to read submodule intervals file");

            for (interval, chunk_type) in submodule_intervals {
                global_intervals
                    .insert_merge_touching_if_values_equal(interval, chunk_type)
                    .expect("to insert interval into global intervals map");
            }
        }
        global_intervals
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
                    .map(|(bytes, chunk_type)| (bytes.clone(), chunk_type.clone()));
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
                        chunk_map
                            .insert(partition_chunk_offset, (bytes, interval_chunk_type.clone()));
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
        let mut buf = vec![0u8; chunk_size as usize];

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
                let _ = set.insert_merge_touching_or_overlapping(interval.clone());
            }
        }
        drop(intervals);

        // Also loop though pending write for matching chunks
        let pending = self
            .pending_writes
            .read()
            .expect("to be able to read pending writes data");

        // If chunk_type is uninitialized, we need to filter out any offsets with pending writes
        if chunk_type == ChunkType::Uninitialized {
            // Remove any offsets from set that have pending writes of any type
            for (offset, _) in pending.iter() {
                // Create a point interval for the offset and remove it from the set
                let point_interval = ii(*offset, *offset);
                let _ = set.cut(point_interval);
            }
        } else {
            // Add chunks from pending_writes with matching chunk_type
            for (offset, (_bytes, pending_chunk_type)) in pending.iter() {
                if *pending_chunk_type == chunk_type {
                    // Create a proper interval for a single chunk & insert it
                    let interval = partition_chunk_offset_ii!(*offset, *offset);
                    let _ = set.insert_merge_touching_or_overlapping(interval);
                }
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
    ) -> () {
        let mut pending = self.pending_writes.write().unwrap();
        pending.insert(chunk_offset, (bytes, chunk_type));
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
        tx_path: TxPath,
        data_root: DataRoot,
        chunk_range: LedgerChunkRange,
        data_size: u64,
    ) -> eyre::Result<()> {
        let storage_range = self.get_storage_module_ledger_range()?;
        let tx_path_hash = H256::from(hash_sha256(&tx_path).unwrap());

        let overlap = storage_range
            .intersection(&chunk_range)
            .ok_or_else(|| eyre::eyre!("chunk_range does not overlap storage module range"))?;

        // Compute the partition relative overlapping chunk range
        let partition_overlap = self.make_range_partition_relative(overlap)?;
        // Compute the Partition relative offset
        let relative_offset =
            RelativeChunkOffset::from(self.make_offset_partition_relative(chunk_range.start())?);

        for (interval, submodule) in self.submodules.overlapping(partition_overlap) {
            let _ = submodule.db.update(|tx| -> eyre::Result<()> {
                // Because each submodule index receives a copy of the path, we need to clone it
                add_full_tx_path(tx, tx_path_hash, tx_path.clone())?;
                set_data_size_for_data_root(tx, data_root, data_size)?;
                if let Some(range) = interval.intersection(&partition_overlap) {
                    // Add the tx_path_hash to every offset in the intersecting range
                    for offset in *range.start()..=*range.end() {
                        let part_offset = PartitionChunkOffset::from(offset);
                        add_tx_path_hash_to_offset_index(
                            tx,
                            part_offset,
                            Some(tx_path_hash.clone()),
                        )?;
                    }
                    // Also update the start offset by data_root index
                    add_start_offset_to_data_root_index(tx, data_root, relative_offset)?;
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
            submodule.db.update(|tx| -> eyre::Result<()> {
                add_full_data_path(tx, data_path_hash, data_path)?;
                add_data_path_hash_to_offset_index(tx, partition_offset, Some(data_path_hash))?;
                Ok(())
            })?
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
        let start_offsets = self.collect_start_offsets(chunk.data_root)?;

        if start_offsets.0.is_empty() {
            return Err(eyre::eyre!("Chunks data_root not found in storage module"));
        }

        let intervals = self.intervals.read().unwrap();

        Ok(start_offsets
            .0
            .iter()
            .map(|offset| PartitionChunkOffset::from(*offset + (*chunk.tx_offset as i32)))
            .filter(|partition_offset| {
                intervals
                    .get_at_point(*partition_offset)
                    .is_some_and(|s| *s == ChunkType::Entropy)
            })
            .collect())
    }

    /// Writes chunk data and its data_path to relevant storage locations
    pub fn write_data_chunk(&self, chunk: &UnpackedChunk) -> eyre::Result<()> {
        let data_path = &chunk.data_path.0;
        let data_path_hash = UnpackedChunk::hash_data_path(data_path);

        // Get all the places this chunks data_root starts in the partition
        let start_offsets = self.collect_start_offsets(chunk.data_root)?;

        if start_offsets.0.is_empty() {
            return Err(eyre::eyre!("Chunks data_root not found in storage module"));
        }

        // Lists for both types of offsets to process
        let mut writeable_offsets = vec![];
        let mut pending_offsets = vec![];

        // Scan all potential locations and categorize them
        for start_offset in start_offsets.0 {
            let partition_offset =
                PartitionChunkOffset::from(start_offset + (*chunk.tx_offset as i32));

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
                .map_or(false, |(_, chunk_type)| *chunk_type == ChunkType::Entropy)
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

    /// Internal helper function to find all the RelativeStartOffsets for a data_root
    /// in this StorageModule
    pub fn collect_start_offsets(&self, data_root: DataRoot) -> eyre::Result<RelativeStartOffsets> {
        let mut offsets = RelativeStartOffsets::default();
        for (_, submodule) in self.submodules.iter() {
            if let Some(rel_offsets) = submodule
                .db
                .view(|tx| get_start_offsets_by_data_root(tx, data_root))??
            {
                offsets.0.extend(rel_offsets.0);
            }
        }
        Ok(offsets)
    }

    pub fn generate_full_chunk_ledger_offset(
        &self,
        ledger_offset: LedgerChunkOffset,
    ) -> Result<Option<PackedChunk>> {
        let range = self.get_storage_module_ledger_range()?;
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
        let (data_root, data_size, data_path, chunk_offset) =
            self.query_submodule_db_by_offset(partition_offset, |tx| {
                let tx_path = get_tx_path_by_offset(tx, partition_offset)?
                    .ok_or(eyre::eyre!("Unable to find a chunk with that tx_path"))?;

                let path_buff = Base64::from(tx_path);
                let proof = get_leaf_proof(&path_buff)?;
                let data_root = proof
                    .hash()
                    .map(H256::from)
                    .ok_or_eyre("Unable to parse data_root from tx_path")?;

                let data_size = get_data_size_by_data_root(tx, data_root)?.ok_or(eyre!(
                    "Unable to get data_size for data_root {}",
                    &data_root
                ))?;

                let data_path = get_data_path_by_offset(tx, partition_offset)?
                    .ok_or(eyre::eyre!("Unable to find a chunk for that data_path"))?;
                let path_buff = Base64::from(data_path);
                let proof = get_leaf_proof(&path_buff)?;
                // -1 as it starts with 0
                let chunk_offset =
                    (proof.offset() as u64).div_ceil(self.config.consensus.chunk_size) - 1;

                Ok((
                    data_root,
                    data_size,
                    path_buff,
                    TxChunkOffset(chunk_offset.try_into().expect("Value exceeds u32::MAX")),
                ))
            })?;

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
        S: FnOnce(&reth_db::mdbx::tx::Tx<reth_db::mdbx::RO>) -> eyre::Result<R>,
    {
        let (_, submodule) = self.get_submodule_for_offset(chunk_offset)?;
        submodule.db.view(fetch_from_db)?
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
    ///
    /// Process:
    /// 1. Locates correct submodule for chunk offset
    /// 2. Calculates physical storage position
    /// 3. Writes chunk data to disk
    /// 4. Updates interval tracking with new chunk state
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
        let (interval, submodule) = self.get_submodule_for_offset(chunk_offset).unwrap();

        // Get the submodule relative offset of the chunk
        let submodule_offset = chunk_offset - interval.start();
        {
            // Lock to the submodules internal file handle & write the chunk

            let mut file = submodule.file.lock().unwrap();

            file.seek(SeekFrom::Start(u64::from(submodule_offset) * chunk_size))?;
            let result = file.write(bytes.as_slice());
            match result {
                // TODO: better logging
                Ok(_bytes_written) => {
                    //info!("write_chunk_internal() -> bytes_written: {}", bytes_written)
                }
                Err(err) => info!("{:?}", err),
            }
            drop(file);
        }

        // If successful, update the StorageModules interval state
        let mut intervals = self.intervals.write().unwrap();

        let chunk_interval = ii(chunk_offset, chunk_offset);
        let _ = intervals.cut(chunk_interval);
        let _ = intervals.insert_merge_touching_if_values_equal(chunk_interval, chunk_type);

        drop(intervals);

        Ok(())
    }

    /// Utility method asking the StorageModule to return its chunk range in
    /// ledger relative coordinates
    pub fn get_storage_module_ledger_range(&self) -> eyre::Result<LedgerChunkRange> {
        let pa = self.partition_assignment.read().unwrap();
        if let Some(part_assign) = *pa {
            if let Some(slot_index) = part_assign.slot_index {
                let start = slot_index as u64 * self.config.consensus.num_chunks_in_partition;
                let end = start + self.config.consensus.num_chunks_in_partition;
                return Ok(LedgerChunkRange(ledger_chunk_offset_ie!(start, end)));
            } else {
                return Err(eyre::eyre!("Ledger slot not assigned!"));
            }
        } else {
            return Err(eyre::eyre!("Partition not assigned!"));
        }
    }

    /// Internal utility function to take a ledger relative range and make it
    /// Partition relative (relative to the partition assigned to the
    /// StorageModule)
    fn make_range_partition_relative(
        &self,
        chunk_range: LedgerChunkRange,
    ) -> eyre::Result<PartitionChunkRange> {
        let storage_module_range = self.get_storage_module_ledger_range()?;
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
        let storage_module_range = self.get_storage_module_ledger_range()?;
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
        let entropy_bytes = vec![0u8; self.config.consensus.chunk_size as usize];
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
        .open(&intervals_path)
        .wrap_err_with(|| {
            format!(
                "Failed to create or open intervals file at {}",
                intervals_path.display()
            )
        })?;

    let file_size = file.metadata()?.len();
    if file_size == 0 {
        let mut file = get_atomic_file(&intervals_path)?;
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
        .open(&path)
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
        .open(path)
        .unwrap_or_else(|_| panic!("Failed to open: {}", path.display()));

    info_file.write_all(serde_json::to_string_pretty(&*info)?.as_bytes())?;
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
            module
                .partition_assignment
                .read()
                .unwrap()
                .and_then(|pa| pa.ledger_id)
                .map_or(false, |id| id == ledger as u32)
                && module
                    .get_storage_module_ledger_range()
                    .map_or(false, |range| range.overlaps(tx_chunk_range))
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
            module
                .partition_assignment
                .read()
                .unwrap()
                .and_then(|pa| pa.ledger_id)
                .map_or(false, |id| id == ledger as u32)
                && module
                    .get_storage_module_ledger_range()
                    .map_or(false, |range| range.contains_point(chunk_offset))
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
            invalid_starts.push(left - PartitionChunkOffset::from(1u64))
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
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        ledger_chunk_offset_ii, partition_chunk_offset_ii, ConsensusConfig, NodeConfig,
        StorageSyncConfig, TxChunkOffset, H256,
    };
    use nodit::interval::ii;

    #[test]
    fn storage_module_test() -> eyre::Result<()> {
        let infos = vec![StorageModuleInfo {
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
                ..ConsensusConfig::testnet()
            }),
            base_directory: base_path.clone(),
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        // Verify the packing params file was created in the submodule
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
            entropy_chunk.to_vec(),
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
            data1_chunk.to_vec(),
            ChunkType::Data,
        );
        storage_module.write_chunk(
            PartitionChunkOffset::from(5),
            data2_chunk.to_vec(),
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
                    (data2_chunk.clone(), ChunkType::Data)
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
                (data1_chunk.clone(), ChunkType::Data)
            ),]
        );

        // Load up the intervals from file
        let intervals = StorageModule::load_intervals_from_submodules(&storage_module.submodules);

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
        let intervals = StorageModule::load_intervals_from_submodules(&storage_module.submodules);

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
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 50), "hdd0-test".into()), // 0 to 50 inclusive
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("pending_writes_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 51,
                chunk_migration_depth: 1,
                ..ConsensusConfig::testnet()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 10,
            },
            base_directory: base_path.clone(),
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);
        let chunk_size = config.consensus.chunk_size as usize;

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        // Queue up some entropy chunks in the pending writes queue
        let entropy_bytes = vec![0u8; chunk_size];
        for chunk_offset in 0..10 as u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(chunk_offset),
                entropy_bytes.clone(),
                ChunkType::Entropy,
            );
        }

        // Sync the chunks
        storage_module.sync_pending_chunks()?;

        // Write 9 more entropy chunks
        for chunk_offset in 10..19 as u32 {
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
        let bytes = vec![10u8; chunk_size];
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
        let bytes = vec![20u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(2);
        storage_module.write_chunk(chunk_offset, bytes.clone(), ChunkType::Data);

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
        let bytes = vec![30u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(20);
        storage_module.write_chunk(chunk_offset, bytes.clone(), ChunkType::Entropy);

        {
            // Verify the resulting intervals
            let entropy = storage_module.get_intervals(ChunkType::Entropy);
            assert_eq!(entropy.len(), 3);
            assert_eq!(entropy[0], partition_chunk_offset_ii!(0, 10));
            // entropy[11] is data
            assert_eq!(entropy[1], partition_chunk_offset_ii!(12, 18));
            // entropy[19] is uninitialized
            assert_eq!(entropy[2], partition_chunk_offset_ii!(20, 20));

            let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
            assert_eq!(uninitialized.len(), 2);
            assert_eq!(uninitialized[0], partition_chunk_offset_ii!(19, 19));
            assert_eq!(uninitialized[1], partition_chunk_offset_ii!(21, 50));
        }

        // Test - write a pending data chunk to an uninitialized offset on disk
        let bytes = vec![40u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(19);
        storage_module.write_chunk(chunk_offset, bytes.clone(), ChunkType::Data);
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
        let infos = vec![StorageModuleInfo {
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
                ..ConsensusConfig::testnet()
            }),
            base_directory: base_path.clone(),
            ..NodeConfig::testnet()
        };
        let config = Config::new(node_config);

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

        let _ = storage_module.index_transaction_data(
            tx_path,
            data_root,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            data_size,
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
}
