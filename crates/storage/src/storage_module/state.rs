use std::{
    fs::{create_dir_all, read_to_string, File, OpenOptions},
    io::{Read, Seek as _, SeekFrom, Write},
    path::PathBuf,
    sync::{Arc, RwLock, RwLockWriteGuard},
    thread::sleep,
    time::Duration,
};

use irys_types::{CHUNK_SIZE, NUM_CHUNKS_IN_RECALL_RANGE};
use nodit::{
    interval::{ie, ii},
    InclusiveInterval, Interval, NoditMap, PointType,
};

use serde::{Deserialize, Serialize, Serializer};
use tracing::{debug, error, warn, Level};
use tracing_subscriber::FmtSubscriber;

use super::interval::{IntervalState, IntervalStateWrapped, PackingState};
use crate::{interval::WriteLock, provider::generate_chunk_test_data};
use eyre::eyre;

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Struct representing all the state of a storage module
pub struct StorageModule {
    /// Path of the SM's storage file
    #[serde(skip)]
    pub path: PathBuf,
    /// Non-overlapping interval map representing the states of different segments of the SM
    pub interval_map: NoditMap<u32, Interval<u32>, IntervalStateWrapped>,
    /// capacity (in chunks) allocated to this storage module
    pub capacity: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]

pub struct StorageModuleConfig {
    pub directory_path: PathBuf,
    pub size_bytes: u64,
}

/// SM directory relative paths for data and metadata/state
const SM_STATE_FILE: &str = "state";
const SM_DATA_FILE: &str = "data";

impl StorageModule {
    /// Saves the current storage module state to disk
    pub fn save_to_disk(&self) -> eyre::Result<()> {
        File::create(self.path.join(SM_STATE_FILE))?
            .write_all(serde_json::to_string(&self)?.as_bytes())?;
        Ok(())
    }

    /// creates a new storage module by loading state from disk
    pub fn load_from_disk(path: PathBuf) -> eyre::Result<Self> {
        let s = read_to_string(path.join(SM_STATE_FILE))?;
        let state: Self = serde_json::from_str(s.as_str())?;
        Ok(state)
    }

    /// creates a new storage module if it can't load it from disk
    pub fn new_or_load_from_disk(config: StorageModuleConfig) -> eyre::Result<Self> {
        let StorageModuleConfig {
            directory_path: path,
            size_bytes,
        } = config;
        if path.join(SM_STATE_FILE).exists() {
            // TODO: resize the SM based on the provided size
            return StorageModule::load_from_disk(path);
        }
        StorageModule::create_new(path, size_bytes)
    }

    /// Create a *new SM* instance at the specified path with the specified capacity (in bytes)
    /// the file will be 0-allocated if it doesn't exist
    pub fn create_new(directory_path: PathBuf, capacity_bytes: u64) -> eyre::Result<Self> {
        let capacity_chunks = (capacity_bytes / CHUNK_SIZE) as u32;

        let mut map = NoditMap::new();

        map.insert_strict(
            ie(0, capacity_chunks),
            IntervalStateWrapped::new(IntervalState {
                state: PackingState::Unpacked,
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

        let sm = StorageModule {
            path: directory_path,
            interval_map: map,
            capacity: capacity_chunks,
        };
        sm.save_to_disk()?;
        return Ok(sm);
    }

    /// Acquire a File handle for the SM's path
    pub fn get_handle(&self) -> Result<File, std::io::Error> {
        File::open(&self.path.join(SM_DATA_FILE))
    }

    /// read some chunks with a sm-relative offset
    pub fn read_chunks(
        &self,
        interval: Interval<u32>,
        expected_state: Option<PackingState>,
    ) -> eyre::Result<Vec<[u8; CHUNK_SIZE as usize]>> {
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
        let overlap_iter = self.interval_map.overlapping(interval);

        let mut result = Vec::with_capacity(chunks_to_read.try_into()?);

        for (segment_interval, segment_state) in overlap_iter {
            if expected_state.is_some()
                && std::mem::discriminant(&segment_state.state)
                    != std::mem::discriminant(&expected_state.unwrap())
            {
                return Err(eyre::Report::msg(format!(
                    "Segment {:?} is of unexpected state {:?}",
                    segment_interval, segment_state
                )));
            }
            while chunks_read < chunks_to_read {
                // read 1 chunk
                let mut buf: [u8; CHUNK_SIZE as usize] = [0; CHUNK_SIZE as usize];
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
        &mut self,
        chunks: Vec<[u8; CHUNK_SIZE as usize]>,
        interval: Interval<u32>,
        expected_state: PackingState,
        new_state: IntervalState,
    ) -> eyre::Result<()> {
        if interval.end() > self.capacity {
            return Err(eyre::Report::msg(
                "Storage module is too small for write interval",
            ));
        }

        let overlap_iter = self.interval_map.overlapping(interval);

        let chunks_to_write = interval.width() + 1;

        // TODO QUESTION - should we enforce that the chunks vec has to be the same length as the interval's width?
        if chunks.len() < chunks_to_write as usize {
            return Err(eyre::Report::msg("Chunks vec is too small for interval"));
        }

        let mut chunks_written = 0;

        let mut handle = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&self.path.join(SM_DATA_FILE))?;

        // move the handle to the requested start
        if interval.start() > 0 {
            handle.seek(SeekFrom::Start(interval.start() as u64 * CHUNK_SIZE))?;
        }

        for (segment_interval, segment_state) in overlap_iter {
            if std::mem::discriminant(&segment_state.state)
                != std::mem::discriminant(&expected_state)
            {
                return Err(eyre::Report::msg(format!(
                    "Segment {:?} is of unexpected state {:?}",
                    segment_interval, segment_state.state
                )));
            }

            while chunks_written < chunks_to_write {
                // TODO @JesseTheRobot use vectored read and write
                let written_bytes = handle.write(chunks.get(chunks_written as usize).unwrap())?;
                debug!(
                    "written bytes {:?} to {:?} (pos: {:?})",
                    &written_bytes,
                    &interval,
                    &handle.stream_position()?
                );
                chunks_written = chunks_written + 1;
            }
        }
        let _ = self
            .interval_map
            .insert_overwrite(interval, IntervalStateWrapped::new(new_state));

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

#[test]
fn test_sm_rw() -> eyre::Result<()> {
    FmtSubscriber::builder().compact().init();

    let chunks: u8 = 20;
    generate_chunk_test_data("/tmp/sample".into(), chunks, 0)?;

    // let sm1_interval_map = NoditMap::from_slice_strict([(
    //     ie(0, chunks as u32),
    //     IntervalStateWrapped::new(IntervalState {
    //         state: PackingState::Data,
    //     }),
    // )])
    // .unwrap();

    // let sm1 = Arc::new(StorageModule::new(
    //     "/tmp/sample".into(),
    //     100 * CHUNK_SIZE,
    //     1,
    // ));

    // let read1 = sm1.read_chunks(ii(0, 5), Some(PackingState::Unpacked))?;

    // let sm2 = sm1.clone();

    // std::thread::spawn(move || {
    //     sm2.write_chunks(
    //         vec![[69; CHUNK_SIZE as usize], [70; CHUNK_SIZE as usize]],
    //         ii(4, 5),
    //         PackingState::Unpacked,
    //         IntervalState {
    //             state: PackingState::Packed,
    //         },
    //     )
    //     .unwrap();
    // });

    // sleep(Duration::from_millis(1000));

    // let read2 = sm1.read_chunks(ii(0, 5), None)?;
    // dbg!(&read2, &read1);
    // let mut sp = StorageProvider {
    //     sm_map: NoditMap::from_slice_strict([(
    //         ie(0, chunks as u32),
    //         StorageModule {
    //             path: "/tmp/sample".into(),
    //             interval_map: RwLock::new(sm1_interval_map),
    //             capacity: chunks as u32,
    //         },
    //     )])
    //     .unwrap(),
    // };

    // let sm = StorageModule {
    //     path: "/tmp/sample".into(),
    //     interval_map: RwLock::new(sm1_interval_map),
    //     capacity: chunks as u32,
    // };

    // let interval = ii(1, 1);

    // let chunks = sm.read_chunks(interval)?;

    // sm.write_chunks(
    //     vec![[69; CHUNK_SIZE as usize]],
    //     interval,
    //     IntervalStateWrapped::new(IntervalState {
    //         state: PackingState::Packed,
    //     }),
    // )?;

    // let chunks2 = sm.read_chunks(interval)?;

    Ok(())
}

// #[test]
// fn vec_test() {
//     let chunks_per_recall_range = NUM_CHUNKS_IN_RECALL_RANGE;
//     let chunks  = 100;
//     let intervals =
//     let mut intervals = vec![];
// }
