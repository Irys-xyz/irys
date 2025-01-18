use derive_more::derive::{Deref, DerefMut};
use eyre::{eyre, OptionExt, Result};
use irys_database::{
    submodule::{
        add_data_path_hash_to_offset_index, add_full_data_path, add_full_tx_path,
        add_start_offset_to_data_root_index, add_tx_path_hash_to_offset_index,
        create_or_open_submodule_db, get_data_path_by_offset, get_start_offsets_by_data_root,
        get_tx_path_by_offset, tables::RelativeStartOffsets,
    },
    Ledger,
};
use irys_packing::packing_xor_vec_u8;
use irys_types::{
    app_state::DatabaseProvider,
    get_leaf_proof,
    partition::{PartitionAssignment, PartitionHash},
    Base64, ChunkBytes, ChunkDataPath, ChunkPathHash, DataRoot, LedgerChunkOffset,
    LedgerChunkRange, PackedChunk, PartitionChunkOffset, PartitionChunkRange, ProofDeserialize,
    StorageConfig, TxPath, TxRelativeChunkOffset, UnpackedChunk, CONFIG, H256,
};
use nodit::{
    interval::{ie, ii},
    InclusiveInterval, Interval, NoditMap, NoditSet,
};
use openssl::sha;
use reth_db::Database;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};
use tracing::{debug, info};

type SubmodulePath = String;

// In-memory chunk data indexed by offset within partition
type ChunkMap = BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>;

/// Storage submodules mapped to their chunk ranges
type SubmoduleMap = NoditMap<u32, Interval<u32>, StorageSubmodule>;

/// Tracks storage state of chunk ranges across all submodules
type StorageIntervals = NoditMap<u32, Interval<u32>, ChunkType>;

/// Maps a logical partition (fixed size) to physical storage across multiple drives
#[derive(Debug)]
pub struct StorageModule {
    /// an integer uniquely identifying the module
    pub id: usize,
    /// The (Optional) info about a partition assigned to this storage module
    pub partition_assignment: Option<PartitionAssignment>,
    /// In-memory chunk buffer awaiting disk write
    pending_writes: Arc<RwLock<ChunkMap>>,
    /// Tracks the storage state of each chunk across all submodules
    intervals: Arc<RwLock<StorageIntervals>>,
    /// Physical storage locations indexed by chunk ranges
    submodules: SubmoduleMap,
    /// Runtime configuration parameters
    pub storage_config: StorageConfig,
    /// Persistent file handle
    intervals_file: Arc<Mutex<File>>,
}

/// On-disk metadata for StorageModule persistence
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageModuleInfo {
    /// An integer uniquely identifying the module
    pub id: usize,
    /// Hash of partition this storage module belongs to, if assigned
    pub partition_assignment: Option<PartitionAssignment>,
    /// Range of chunk offsets and path for each submodule
    pub submodules: Vec<(Interval<u32>, SubmodulePath)>,
}

/// Manages chunk storage on a single physical drive
#[derive(Debug)]
pub struct StorageSubmodule {
    /// Persistent storage handle
    file: Arc<Mutex<File>>,
    /// Persistent database env
    pub db: DatabaseProvider,
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
    pub fn new(
        base_path: &PathBuf,
        storage_module_info: &StorageModuleInfo,
        storage_config: StorageConfig,
    ) -> eyre::Result<Self> {
        let mut map = NoditMap::new();
        let mut intervals = StorageIntervals::new();

        for (interval, dir) in storage_module_info.submodules.clone() {
            let sub_base_path = base_path.join(dir);
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
            let submodule_db = create_or_open_submodule_db(&submodule_db_path).map_err(|e| {
                eyre!(
                    "Failed to open submodule database: {} - {}",
                    submodule_db_path.display(),
                    e
                )
            })?;

            map.insert_strict(
                interval.clone(),
                StorageSubmodule {
                    file: chunks_file,
                    db: DatabaseProvider(Arc::new(submodule_db)),
                },
            )
            .map_err(|e| {
                eyre!(
                    "Failed to insert submodule over interval: {}-{}, {:?}",
                    interval.start(),
                    interval.end(),
                    e
                )
            })?;

            let _ =
                intervals.insert_merge_touching_if_values_equal(interval, ChunkType::Uninitialized);
        }

        // TODO: if there are any gaps, or the range doesn't cover a full module range panic

        let gaps = intervals
            .gaps_untrimmed(ii(0, u32::MAX))
            .collect::<Vec<_>>();
        let expected = vec![ii(storage_config.num_chunks_in_partition as u32, u32::MAX)];
        if &gaps != &expected {
            return Err(eyre!(
                "Invalid storage module config, expected range {:?}, got range {:?}",
                &expected,
                &gaps
            ));
        }

        let path = base_path.join(format!(
            "StorageModule_{}_intervals.json",
            storage_module_info.id
        ));

        let intervals_file = Arc::new(Mutex::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true) // Optional: creates file if it doesn't exist
                .open(&path)
                .map_err(|e| {
                    eyre!(
                        "Failed to create or open interval file: {} - {}",
                        path.display(),
                        e
                    )
                })?,
        ));

        // Attempt to restore intervals from the intervals file.
        if let Ok(ints) = read_intervals_file(intervals_file.clone()) {
            intervals = ints;
        }

        Ok(StorageModule {
            id: storage_module_info.id,
            partition_assignment: storage_module_info.partition_assignment,
            pending_writes: Arc::new(RwLock::new(ChunkMap::new())),
            intervals: Arc::new(RwLock::new(intervals)),
            submodules: map,
            storage_config,
            intervals_file,
        })
    }

    /// Returns the StorageModules partition_hash if assigned
    pub fn partition_hash(&self) -> Option<PartitionHash> {
        if let Some(part_assign) = self.partition_assignment {
            Some(part_assign.partition_hash)
        } else {
            None
        }
    }

    /// Returns whether the given chunk offset falls within this StorageModules assigned range
    pub fn contains_offset(&self, chunk_offset: LedgerChunkOffset) -> bool {
        self.partition_assignment
            .and_then(|part| part.slot_index)
            .map(|slot_index| {
                let start_offset = slot_index as u64 * self.storage_config.num_chunks_in_partition;
                let end_offset = start_offset + self.storage_config.num_chunks_in_partition;
                (start_offset..end_offset).contains(&chunk_offset)
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
        let threshold = self.storage_config.min_writes_before_sync;
        let arc = self.pending_writes.clone();

        // First use read lock to check if we have work to do
        let write_batch = {
            let pending = arc.read().unwrap();
            self.submodules
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
                .collect::<Vec<_>>()
        }; // Read lock released here

        // Only acquire write lock if we have work to do
        if !write_batch.is_empty() {
            let mut pending = arc.write().unwrap();
            for (chunk_offset, (bytes, chunk_type)) in write_batch {
                self.write_chunk_internal(chunk_offset, bytes, chunk_type.clone())?;
                pending.remove(&chunk_offset); // Clean up written chunks

                // update the storage intervals
                {
                    let ie = ii(chunk_offset, chunk_offset);
                    let mut intervals = self.intervals.write().unwrap();
                    let _ = intervals.insert_overwrite(ie, chunk_type);
                }
            }

            {
                // Save the updated intervals
                let mut file = self.intervals_file.lock().unwrap();
                let intervals = self.intervals.read().unwrap();
                file.set_len(0)?;
                file.seek(SeekFrom::Start(0))?;
                file.write_all(serde_json::to_string(&*intervals)?.as_bytes())?;
            }
        }

        Ok(())
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
        let iter = intervals.overlapping(chunk_range);
        for (interval, chunk_type) in iter {
            if *chunk_type != ChunkType::Uninitialized {
                // For each chunk in the interval
                for chunk_offset in interval.start()..=interval.end() {
                    // Read the chunk from disk
                    let bytes = self.read_chunk_internal(chunk_offset)?;

                    // Add it to the ChunkMap
                    chunk_map.insert(chunk_offset, (bytes, chunk_type.clone()));
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
        let chunk_size = self.storage_config.chunk_size as u32;
        let file_offset = (chunk_offset - interval.start()) * chunk_size;
        let mut buf = vec![0u8; chunk_size as usize];

        // Read chunk from file
        let mut file = submodule.file.lock().unwrap();
        file.seek(SeekFrom::Start(file_offset.into()))?;
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
    pub fn get_intervals(&self, chunk_type: ChunkType) -> Vec<Interval<u32>> {
        let intervals = self.intervals.read().unwrap();
        let mut set = NoditSet::new();
        for (interval, ct) in intervals.iter() {
            if *ct == chunk_type {
                let _ = set.insert_merge_touching_or_overlapping(interval.clone());
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
        // Add the chunk to pending writes
        let mut pending = self.pending_writes.write().unwrap();
        pending.insert(chunk_offset, (bytes, chunk_type));
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
    ) -> eyre::Result<()> {
        let storage_range = self.get_storage_module_range()?;
        let tx_path_hash = H256::from(hash_sha256(&tx_path).unwrap());

        let overlap = storage_range
            .intersection(&chunk_range)
            .ok_or_else(|| eyre::eyre!("chunk_range does not overlap storage module range"))?;

        // Compute the partition relative overlapping chunk range
        let partition_overlap = self.make_range_partition_relative(overlap)?;
        // Compute the Partition relative offset
        let relative_offset = self.make_offset_partition_relative(chunk_range.start())?;

        for (interval, submodule) in self.submodules.overlapping(partition_overlap) {
            let _ = submodule.db.update(|tx| -> eyre::Result<()> {
                // Because each submodule index receives a copy of the path, we need to clone it
                add_full_tx_path(tx, tx_path_hash, tx_path.clone())?;

                if let Some(range) = interval.intersection(&partition_overlap) {
                    // Add the tx_path_hash to every offset in the intersecting range
                    for offset in range.start()..=range.end() {
                        add_tx_path_hash_to_offset_index(tx, offset, Some(tx_path_hash.clone()))?;
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

    /// Gets the list of partition-relative offsets in this partition that the chunk should be written to
    pub fn get_write_offsets(
        &self,
        chunk: &UnpackedChunk,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let start_offsets = self.collect_start_offsets(chunk.data_root)?;

        if start_offsets.0.len() == 0 {
            return Err(eyre::eyre!("Chunks data_root not found in storage module"));
        }

        let mut write_offsets = vec![];
        for start_offset in start_offsets.0 {
            let partition_offset = (start_offset + chunk.tx_offset as i32)
                .try_into()
                .map_err(|_| eyre::eyre!("Invalid negative offset: {}", chunk.tx_offset))?;

            {
                // read the metadata in a block so the read guard expires quickly
                let intervals = self.intervals.read().unwrap();
                let chunk_state = intervals.get_at_point(partition_offset);
                if chunk_state.is_some_and(|s| *s == ChunkType::Entropy) {
                    write_offsets.push(partition_offset)
                }
            };
        }

        Ok(write_offsets)
    }

    /// Writes chunk data and its data_path to relevant storage locations
    pub fn write_data_chunk(&self, chunk: &UnpackedChunk) -> eyre::Result<()> {
        let data_path = &chunk.data_path.0;
        let data_path_hash = UnpackedChunk::hash_data_path(data_path);

        for partition_offset in self.get_write_offsets(chunk)? {
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
    pub fn generate_full_chunk(
        &self,
        ledger_offset: LedgerChunkOffset,
    ) -> Result<Option<PackedChunk>> {
        // Get paths and process them
        let (tx_path, data_path) = self.read_tx_data_path(ledger_offset)?;

        let (data_root, data_size) = match tx_path {
            Some(tp) => {
                let path_buff = Base64::from(tp);
                let proof = get_leaf_proof(&path_buff)?;
                let data_root = proof
                    .hash()
                    .map(H256::from)
                    .ok_or_eyre("Unable to parse data_root from tx_path ")?;
                let data_size = proof.offset() as u64;
                (data_root, data_size)
            }
            None => return Err(eyre::eyre!("Unable to find a chunk with that tx_path")),
        };

        let (data_path, _offset) = match data_path {
            Some(dp) => {
                let path_buff = Base64::from(dp);
                let proof = get_leaf_proof(&path_buff)?;
                (path_buff, proof.offset() as u64)
            }
            None => return Err(eyre::eyre!("Unable to find a chunk for that data_path")),
        };

        // Get chunk info and calculate index
        let range = self.get_storage_module_range()?;
        let partition_offset = (ledger_offset - range.start()) as u32;
        let closest_offsets = self.collect_start_offsets(data_root)?;

        let nearest_start_offset = closest_offsets
            .0
            .iter()
            .filter(|&&offset| offset <= partition_offset as i32)
            .max()
            .copied()
            .ok_or_eyre("Could not find nearest_start_offset")?;

        let chunks = self.read_chunks(ii(partition_offset, partition_offset))?;
        let chunk_info = chunks
            .get(&partition_offset)
            .ok_or_eyre("Could not find chunk bytes on disk")?;

        // Because nearest_start_offset can be negative (for data_roots that
        // overlap partition boundaries) we do our calculations with i64s to
        // account for negative nearest_start_offset
        let data_root_start_offset: LedgerChunkOffset =
            (range.start() as i64 + nearest_start_offset as i64) as u64;

        // Finally the index of the chunk in the transaction can be calculated
        // using the ledger relative start_offset of the data_root and the
        // ledger_offset provided by the caller
        let chunk_offset = (ledger_offset - data_root_start_offset) as TxRelativeChunkOffset;

        Ok(Some(PackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64::from(chunk_info.0.clone()),
            partition_offset,
            tx_offset: chunk_offset,
            packing_address: self.storage_config.miner_address,
            partition_hash: self.partition_hash().unwrap(),
        }))
    }

    /// Gets the tx_path and data_path for a chunk using its ledger relative offset
    pub fn read_tx_data_path(
        &self,
        chunk_offset: LedgerChunkOffset,
    ) -> eyre::Result<(Option<TxPath>, Option<ChunkDataPath>)> {
        let (_interval, submodule) = self
            .submodules
            .get_key_value_at_point(chunk_offset as u32)
            .unwrap();

        submodule.db.view(|tx| {
            Ok((
                get_tx_path_by_offset(tx, chunk_offset as u32)?,
                get_data_path_by_offset(tx, chunk_offset as u32)?,
            ))
        })?
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
        let chunk_size = self.storage_config.chunk_size;
        // Get the correct submodule reference based on chunk_offset
        let (interval, submodule) = self
            .submodules
            .get_key_value_at_point(chunk_offset)
            .unwrap();

        // Get the submodule relative offset of the chunk
        let submodule_offset = chunk_offset - interval.start();
        {
            // Lock to the submodules internal file handle & write the chunk
            let mut file = submodule.file.lock().unwrap();
            file.seek(SeekFrom::Start(submodule_offset as u64 * chunk_size))?;
            let result = file.write(bytes.as_slice());
            match result {
                // TODO: better logging
                Ok(_bytes_written) => {
                    //info!("write_chunk_internal() -> bytes_written: {}", bytes_written)
                }
                Err(err) => info!("{:?}", err),
            }
        }

        // If successful, update the StorageModules interval state
        let mut intervals = self.intervals.write().unwrap();
        let chunk_interval = ii(chunk_offset, chunk_offset);
        let _ = intervals
            .insert_merge_touching_if_values_equal(chunk_interval, chunk_type.clone())
            .unwrap_or_else(|_| {
                let _ = intervals.insert_overwrite(chunk_interval, chunk_type);
                chunk_interval // Return original interval, but it's discarded by outer _
            });
        Ok(())
    }

    /// Utility method asking the StorageModule to return its chunk range in
    /// ledger relative coordinates
    pub fn get_storage_module_range(&self) -> eyre::Result<LedgerChunkRange> {
        if let Some(part_assign) = self.partition_assignment {
            if let Some(slot_index) = part_assign.slot_index {
                let start = slot_index as u64 * self.storage_config.num_chunks_in_partition;
                let end = start + self.storage_config.num_chunks_in_partition;
                return Ok(LedgerChunkRange(ie(start, end)));
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
        let storage_module_range = self.get_storage_module_range()?;
        let start = chunk_range.start() - storage_module_range.start();
        let end = chunk_range.end() - storage_module_range.start();
        Ok(PartitionChunkRange(ii(start as u32, end as u32)))
    }

    /// utility function to take a ledger relative offset and makes it
    /// Partition relative (relative to the partition assigned to the
    /// StorageModule)
    pub fn make_offset_partition_relative(
        &self,
        start_offset: LedgerChunkOffset,
    ) -> eyre::Result<i32> {
        let storage_module_range = self.get_storage_module_range()?;
        let start = start_offset as i64 - storage_module_range.start() as i64;
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
        Ok(local_offset.try_into()?)
    }

    /// Test utility function to mark a StorageModule as packed
    pub fn pack_with_zeros(&self) {
        let entropy_bytes = vec![0u8; self.storage_config.chunk_size as usize];
        for chunk_offset in 0..self.storage_config.num_chunks_in_partition as u32 {
            self.write_chunk(chunk_offset, entropy_bytes.clone(), ChunkType::Entropy);
            self.sync_pending_chunks().unwrap();
        }
    }
}

/// Creates required storage directory structure and empty data files
///
/// Creates:
/// - Base directory
/// - Subdirectories for each range
/// - Empty chunks.dat files in each subdirectory
///
/// Deletes:
/// - the _intervals.json, resetting the storage module state
///
/// Used primarily for testing storage initialization
pub fn initialize_storage_files(base_path: &PathBuf, infos: &Vec<StorageModuleInfo>) -> Result<()> {
    debug!(target: "irys::storage_module", base_path=?base_path, "Initializing storage files" );
    // Create base storage directory if it doesn't exist
    fs::create_dir_all(base_path.clone())?;

    for (idx, info) in infos.iter().enumerate() {
        // Create subdirectories for each range
        for (_, dir) in info.submodules.clone() {
            let path = base_path.join(dir);
            fs::create_dir_all(&path)?;

            // Create empty data file if it doesn't exist
            let data_file = path.join("chunks.dat");
            if !data_file.exists() {
                fs::File::create(data_file)?;
            }
        }

        // Create a StorageModuleInfo file in the base path for each module
        let info_path = base_path.join(format!("StorageModule_{}.json", idx));
        write_info_file(&info_path, &info).unwrap();

        let path = format!(
            "{}StorageModule_{}_intervals.json",
            base_path.display(),
            infos[0].id
        );
        let path = Path::new(&path);
        if path.exists() && !CONFIG.persist_data_on_restart {
            fs::remove_file(path).unwrap();
        }
    }

    Ok(())
}

/// Reads and deserializes intervals from storage state file
///
/// Loads the stored interval mapping that tracks chunk states.
/// Expects a JSON-formatted file containing StorageIntervals.
pub fn read_intervals_file(intervals_file: Arc<Mutex<File>>) -> eyre::Result<StorageIntervals> {
    let mut file = intervals_file.lock().unwrap();
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

    info_file.write_all(serde_json::to_string(&*info)?.as_bytes())?;
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
    storage_modules: &[Arc<StorageModule>],
    ledger: Ledger,
    tx_chunk_range: &LedgerChunkRange,
) -> Vec<Arc<StorageModule>> {
    storage_modules
        .iter()
        .filter(|module| {
            module
                .partition_assignment
                .and_then(|pa| pa.ledger_num)
                .map_or(false, |num| num == ledger as u64)
                && module
                    .get_storage_module_range()
                    .map_or(false, |range| range.overlaps(tx_chunk_range))
        })
        .cloned() // Clone the Arc, which is cheap
        .collect()
}

/// For a given ledger and ledger offset this function attempts to find
/// a storage module that overlaps the offset
pub fn get_storage_module_at_offset(
    storage_modules: &[Arc<StorageModule>],
    ledger: Ledger,
    chunk_offset: LedgerChunkOffset,
) -> Option<Arc<StorageModule>> {
    storage_modules
        .iter()
        .find(|module| {
            module
                .partition_assignment
                .and_then(|pa| pa.ledger_num)
                .map_or(false, |num| num == ledger as u64)
                && module
                    .get_storage_module_range()
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

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::H256;
    use nodit::interval::ii;

    #[test]
    fn storage_module_test() -> eyre::Result<()> {
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![
                (ii(0, 4), "hdd0-4TB".to_string()),  // 0 to 4 inclusive
                (ii(5, 9), "hdd1-4TB".to_string()),  // 5 to 9 inclusive
                (ii(10, 19), "hdd-8TB".to_string()), // 10 to 19 inclusive
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("storage_module_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let _ = initialize_storage_files(&base_path, &infos);

        // Verify the StorageModuleInfo file was crated in the base path
        let file_infos = read_info_file(&base_path.join("StorageModule_0.json")).unwrap();
        assert_eq!(file_infos, infos[0]);

        // Override the default StorageModule config for testing
        let config = StorageConfig {
            min_writes_before_sync: 1,
            chunk_size: 32,
            num_chunks_in_partition: 20,
            ..Default::default()
        };

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(&base_path, storage_module_info, config)?;

        // Verify the entire storage module range is uninitialized
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [ii(0, 19)]);

        // Create a test (fake) entropy chunk
        let entropy_chunk = vec![0xff; 32]; // All bytes set to 0xff
        storage_module.write_chunk(1, entropy_chunk.to_vec(), ChunkType::Entropy);

        // Invoke the sync task so it gets written to disk
        let _ = storage_module.sync_pending_chunks();

        // Validate the uninitialized intervals have been updated to reflect the new chunk
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [ii(0, 0), ii(2, 19)]);

        // Validate the Entropy (Packed/unsynced) intervals have been updated
        let packed = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(packed, [ii(1, 1)]);

        // Validate entropy chunk can be read after writing
        let chunks = storage_module.read_chunks(ii(1, 1)).unwrap();
        let chunk = chunks.get(&1).unwrap();
        assert_eq!(*chunk, (entropy_chunk.clone(), ChunkType::Entropy));

        // Validate that uninitialized chunks are not returned by read_chunks
        let chunks = storage_module.read_chunks(ii(1, 2)).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(*chunk, (entropy_chunk.clone(), ChunkType::Entropy));

        // Write and sync two sequential data chunks that span a submodule boundary
        let data1_chunk = vec![0x4; 32];
        let data2_chunk = vec![0x5; 32];

        storage_module.write_chunk(4, data1_chunk.to_vec(), ChunkType::Data);
        storage_module.write_chunk(5, data2_chunk.to_vec(), ChunkType::Data);

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
        assert_eq!(data, [ii(4, 5)]);

        // Validate the unpacked intervals are updated
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [ii(0, 0), ii(2, 3), ii(6, 19)]);

        // Validate a read_chunks operation across submodule boundaries
        let chunks = storage_module.read_chunks(ii(4, 5)).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks.into_iter().collect::<Vec<_>>(),
            [
                (4, (data1_chunk.clone(), ChunkType::Data)),
                (5, (data2_chunk.clone(), ChunkType::Data))
            ]
        );

        // Query past the range of the StorageModule
        let chunks = storage_module.read_chunks(ii(0, 25)).unwrap();

        // Verify only initialized chunks are returned
        assert_eq!(
            chunks.into_iter().collect::<Vec<_>>(),
            [
                (1, (entropy_chunk, ChunkType::Entropy)),
                (4, (data1_chunk.clone(), ChunkType::Data)),
                (5, (data2_chunk.clone(), ChunkType::Data))
            ]
        );

        // Load up the intervals from file
        let intervals = read_intervals_file(storage_module.intervals_file.clone()).unwrap();

        let file_intervals = intervals.into_iter().collect::<Vec<_>>();
        let ints = storage_module.intervals.read().unwrap();
        let module_intervals = ints.clone().into_iter().collect::<Vec<_>>();
        assert_eq!(file_intervals, module_intervals);

        Ok(())
    }

    #[test]
    fn data_path_test() -> eyre::Result<()> {
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (ii(0, 4), "hdd0-4TB".to_string()), // 0 to 4 inclusive
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("data_path_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        initialize_storage_files(&base_path, &infos)?;

        // Override the default StorageModule config for testing
        let config = StorageConfig {
            min_writes_before_sync: 1,
            chunk_size: 5,
            num_chunks_in_partition: 5,
            ..Default::default()
        };

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(&base_path, storage_module_info, config)?;
        let chunk_data = vec![0, 1, 2, 3, 4];
        let data_path = vec![4, 3, 2, 1];
        let tx_path = vec![5, 6, 7, 8];
        let data_root = H256::zero();

        // Pack the storage module
        storage_module.pack_with_zeros();

        let _ =
            storage_module.index_transaction_data(tx_path, data_root, LedgerChunkRange(ii(0, 0)));

        let chunk = UnpackedChunk {
            data_root: H256::zero(),
            data_size: chunk_data.len() as u64,
            data_path: data_path.clone().into(),
            bytes: chunk_data.into(),
            tx_offset: 0,
        };

        storage_module.write_data_chunk(&chunk)?;

        let (_, ret_path) = storage_module.read_tx_data_path(0)?;

        assert_eq!(ret_path, Some(data_path));

        Ok(())
    }
}
