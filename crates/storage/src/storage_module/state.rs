use std::{
    fs::{create_dir_all, read_to_string, File, OpenOptions},
    io::{Read, Seek as _, SeekFrom, Write},
    path::PathBuf,
    sync::RwLock,
};

use irys_types::{
    ChunkBin, ChunkState, IntervalState, IntervalStateWrapped, StorageModuleConfig, CHUNK_SIZE,
    NUM_CHUNKS_IN_RECALL_RANGE,
};
use nodit::{
    interval::{ie, ii},
    InclusiveInterval, Interval, NoditMap, PointType,
};

use serde::{Deserialize, Serialize, Serializer};
use tracing::{debug, error, warn, Level};
use tracing_subscriber::FmtSubscriber;

use eyre::eyre;

/// SM directory relative paths for data and metadata/state
const SM_STATE_FILE: &str = "state";
const SM_DATA_FILE: &str = "data";

#[derive(Debug, Serialize, Deserialize)]
/// Struct representing all the state of a storage module
pub struct StorageModule {
    /// Path of the SM's storage file
    #[serde(skip)]
    pub path: PathBuf,
    /// Non-overlapping interval map representing the states of different segments of the SM
    pub interval_map: RwLock<NoditMap<u32, Interval<u32>, IntervalStateWrapped>>,
    /// capacity (in chunks) allocated to this storage module
    pub capacity: u32,
    /// TODO @JesseTheRobot - implement segment locking
    /// Map of fixed width (200MIB) segments to their corresponding rwlock
    // pub lock_map: NoditMap<u32, Interval<u32>, RwLock<()>>,
    /// Saved last configuration
    pub config: StorageModuleConfig,
}

/// Storage modules act as our counterpart to a discrete storage device, one or more of which are used in sequence to build enough storage for a partition
/// Storage modules work off a single large binary file, aligned to CHUNK_SIZE, and work off chunk offsets instead of bytes
impl StorageModule {
    /// Saves the current storage module state to disk
    pub fn save_to_disk(&self) -> eyre::Result<()> {
        File::create(self.path.join(SM_STATE_FILE))?
            .write_all(serde_json::to_string(&self)?.as_bytes())?;
        Ok(())
    }

    /// creates a new storage module by loading state from disk
    pub fn load_from_disk(path: PathBuf) -> eyre::Result<StorageModule> {
        // TODO @JesseTheRobot - validate provided config against stored config
        // perform any operations like resizing etc
        let s = read_to_string(path.join(SM_STATE_FILE))?;
        let state: StorageModule = serde_json::from_str(s.as_str())?;
        Ok(StorageModule {
            path,
            interval_map: state.interval_map,
            capacity: state.capacity,
            config: state.config,
        })
    }

    /// creates a new storage module if it can't load it from disk
    pub fn new_or_load_from_disk(config: StorageModuleConfig) -> eyre::Result<Self> {
        // let StorageModuleConfig {
        //     directory_path: path,
        //     size_bytes,
        //     chunks_per_lock_segment: chunks_per_lock,
        // } = config;
        if config.directory_path.join(SM_STATE_FILE).exists() {
            // TODO: resize the SM based on the provided size
            return StorageModule::load_from_disk(config.directory_path);
        }
        StorageModule::create_new(config)
    }

    /// Create a *new SM* instance at the specified path with the specified capacity (in bytes)
    /// the file will be 0-allocated if it doesn't exist
    pub fn create_new(config: StorageModuleConfig) -> eyre::Result<Self> {
        let StorageModuleConfig {
            directory_path,
            size_bytes,
            // chunks_per_lock_segment,
        } = config.clone();
        let capacity_chunks = (size_bytes / CHUNK_SIZE) as u32;

        let mut map = NoditMap::new();

        map.insert_strict(
            ie(0, capacity_chunks),
            IntervalStateWrapped::new(IntervalState {
                chunk_state: ChunkState::Unpacked,
            }),
        )
        .unwrap();

        // sparse allocate the file
        // some OS's won't 0-fill, but that's fine as we don't expect to be able to always read 0 from uninitialized space
        if !directory_path.join(SM_DATA_FILE).exists() {
            // create the parent folder
            create_dir_all(&directory_path)?;
            let mut file = File::create(directory_path.join(SM_DATA_FILE).clone()).unwrap();
            file.seek(SeekFrom::Start(capacity_chunks as u64 * CHUNK_SIZE))
                .unwrap();
            file.write_all(&[0]).unwrap();
        }
        // create the lock tree, one interval for every N MB
        // let lock_map: NoditMap<u32, Interval<u32>, RwLock<()>> = NoditMap::new();

        let sm = StorageModule {
            path: directory_path,
            interval_map: RwLock::new(map),
            capacity: capacity_chunks,
            config,
            // lock_map,
        };

        sm.save_to_disk()?;
        return Ok(sm);
    }

    /// Acquire a read only File handle for the SM's path
    pub fn get_handle(&self) -> Result<File, std::io::Error> {
        dbg!("Getting handle {}", self.path.clone());
        File::open(&self.path.join(SM_DATA_FILE))
    }

    /// read some chunks with a sm-relative offset
    pub fn read_chunks(
        &self,
        interval: Interval<u32>,
        expected_state: Option<ChunkState>,
    ) -> eyre::Result<Vec<ChunkBin>> {
        if interval.end() > self.capacity {
            return Err(eyre!("Read goes beyond SM capacity!"));
        }

        let chunks_to_read = interval.width() + 1;
        let mut chunks_read: u32 = 0;
        let mut handle = self.get_handle()?;

        // move the handle to the requested start
        if interval.start() > 0 {
            handle.seek(SeekFrom::Start(interval.start() as u64 * CHUNK_SIZE))?;
        }

        debug!("getting read lock {:?}", &interval);
        let map = self.interval_map.read().unwrap();
        debug!("got read lock {:?}", &interval);
        let overlap_iter = map.overlapping(interval);

        let mut result = Vec::with_capacity(chunks_to_read.try_into()?);

        for (segment_interval, segment_state) in overlap_iter {
            if expected_state.is_some()
                && std::mem::discriminant(&segment_state.chunk_state)
                    != std::mem::discriminant(&expected_state.unwrap())
            {
                return Err(eyre::Report::msg(format!(
                    "Segment {:?} is of unexpected state {:?}",
                    segment_interval, segment_state
                )));
            }
            while chunks_read < chunks_to_read {
                // read 1 chunk
                let mut buf: ChunkBin = [0; CHUNK_SIZE as usize];
                debug!(
                    "handle pos: {:?}, path: {:?}",
                    handle.stream_position()?,
                    &self.path
                );

                handle.read_exact(&mut buf)?;
                result.push(buf);
                chunks_read = chunks_read + 1;
            }
        }

        return Ok(result);
    }

    /// Writes some chunks to an interval, and tags the written interval with new state
    pub fn write_chunks(
        &self,
        chunks: Vec<ChunkBin>,
        interval: Interval<u32>,
        expected_state: Option<ChunkState>,
        new_state: IntervalState,
    ) -> eyre::Result<()> {
        if interval.end() > self.capacity {
            return Err(eyre::Report::msg(
                "Storage module is too small for write interval",
            ));
        }
        error!("getting write lock {:?}", &interval);
        let mut map = self.interval_map.write().unwrap();
        error!("got write lock {:?}", &interval);

        let overlap_iter = map.overlapping(interval);

        let chunks_to_write = interval.width() + 1;

        // TODO QUESTION - should we enforce that the chunks vec has to be the same length as the interval's width?
        if chunks.len() < chunks_to_write as usize {
            return Err(eyre::Report::msg("Chunks vec is too small for interval"));
        }

        let mut chunks_written = 0;

        let mut handle = OpenOptions::new()
            .write(true)
            .open(&self.path.join(SM_DATA_FILE))?;

        // move the handle to the requested start
        if interval.start() > 0 {
            handle.seek(SeekFrom::Start(interval.start() as u64 * CHUNK_SIZE))?;
        }

        for (segment_interval, segment_state) in overlap_iter {
            if std::mem::discriminant(&segment_state.chunk_state)
                != std::mem::discriminant(&expected_state.unwrap_or(segment_state.chunk_state))
            {
                return Err(eyre::Report::msg(format!(
                    "Segment {:?} is of unexpected state {:?}",
                    segment_interval, segment_state.chunk_state
                )));
            }

            while chunks_written < chunks_to_write {
                // TODO @JesseTheRobot use vectored read and write
                let written_bytes = handle.write(chunks.get(chunks_written as usize).unwrap())?;
                error!(
                    "written bytes {:?} to {:?} (pos: {:?})",
                    &written_bytes,
                    &interval,
                    &handle.stream_position()?
                );
                chunks_written = chunks_written + 1;
            }
        }
        // let _ = self
        //     .interval_map
        //     .insert_overwrite(interval, IntervalStateWrapped::new(new_state));

        let _cut = map.cut(interval);
        let _ = map
            .insert_merge_touching_if_values_equal(interval, IntervalStateWrapped::new(new_state))
            .unwrap();
        drop(map); // have to drop before saving, otherwise we deadlock
        self.save_to_disk()?;
        Ok(())
    }
}

// /// Resets an interval to `PackingState::Unpacked`
// pub fn reset_range(
//     mut map: RwLockWriteGuard<'_, NoditMap<u32, Interval<u32>, IntervalStateWrapped>>,
//     interval: Interval<u32>,
// ) {
//     let _ = map.insert_overwrite(
//         interval,
//         IntervalStateWrapped::new(IntervalState {
//             state: PackingState::Unpacked,
//         }),
//     );
// }

// TODO @JesseTheRobot re-enable tests
// #[test]
// fn test_sm_rw() -> eyre::Result<()> {
//     FmtSubscriber::builder().compact().init();

//     let chunks: u8 = 20;
//     generate_chunk_test_data("/tmp/sample".into(), chunks, 0)?;

//     // let sm1_interval_map = NoditMap::from_slice_strict([(
//     //     ie(0, chunks as u32),
//     //     IntervalStateWrapped::new(IntervalState {
//     //         state: PackingState::Data,
//     //     }),
//     // )])
//     // .unwrap();

//     // let sm1 = Arc::new(StorageModule::new(
//     //     "/tmp/sample".into(),
//     //     100 * CHUNK_SIZE,
//     //     1,
//     // ));

//     // let read1 = sm1.read_chunks(ii(0, 5), Some(PackingState::Unpacked))?;

//     // let sm2 = sm1.clone();

//     // std::thread::spawn(move || {
//     //     sm2.write_chunks(
//     //         vec![[69; CHUNK_SIZE as usize], [70; CHUNK_SIZE as usize]],
//     //         ii(4, 5),
//     //         PackingState::Unpacked,
//     //         IntervalState {
//     //             state: PackingState::Packed,
//     //         },
//     //     )
//     //     .unwrap();
//     // });

//     // sleep(Duration::from_millis(1000));

//     // let read2 = sm1.read_chunks(ii(0, 5), None)?;
//     // dbg!(&read2, &read1);
//     // let mut sp = StorageProvider {
//     //     sm_map: NoditMap::from_slice_strict([(
//     //         ie(0, chunks as u32),
//     //         StorageModule {
//     //             path: "/tmp/sample".into(),
//     //             interval_map: RwLock::new(sm1_interval_map),
//     //             capacity: chunks as u32,
//     //         },
//     //     )])
//     //     .unwrap(),
//     // };

//     // let sm = StorageModule {
//     //     path: "/tmp/sample".into(),
//     //     interval_map: RwLock::new(sm1_interval_map),
//     //     capacity: chunks as u32,
//     // };

//     // let interval = ii(1, 1);

//     // let chunks = sm.read_chunks(interval)?;

//     // sm.write_chunks(
//     //     vec![[69; CHUNK_SIZE as usize]],
//     //     interval,
//     //     IntervalStateWrapped::new(IntervalState {
//     //         state: PackingState::Packed,
//     //     }),
//     // )?;

//     // let chunks2 = sm.read_chunks(interval)?;

//     Ok(())
// }
